# mobius 0.17.0

This is a breaking Rust API release. Unused extension methods and types have been removed, parent modules are split into focused implementations, shared provider encoding replaces repeated transports, and poisoned locks fail operations instead of recovering state.

Streamed events share their step state, worker exchanges use per-session locks, and checkpoint and context paths avoid repeated work. These changes are not a promise of lower end-to-end latency for every provider or workload.

The SSE parser accepts a leading UTF-8 BOM, including when its bytes arrive separately. WebSocket responses now report citation-related web activity consistently with HTTP. Public checkpoint serialization continues to preserve model context; SQLite still stores its header and context separately.

Provider model override TOML now declares reasoning presets once at catalog level and references their IDs from models. Convert existing custom catalogs before upgrade; invalid overrides are rejected with a warning and use the bundled catalog.
