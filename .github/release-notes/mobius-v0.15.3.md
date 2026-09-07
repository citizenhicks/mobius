Starts authorized tool execution while model output is still streaming, with ordered validation, results, and cancellation. Replaces build-time provider, middleware, and sandbox TOML manifests with Rust-owned declarations, and separates local sandbox enforcement by platform.

Checkpoint format 13 is unchanged. Custom checkpoint backends must implement transcript journals explicitly; compacted checkpoint context is no longer returned as transcript history. Atomic checkpoint/event commit requirements are documented.
