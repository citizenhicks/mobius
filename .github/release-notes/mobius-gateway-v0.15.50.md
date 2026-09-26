# möbius Gateway 0.15.50

Starting a newer gateway now gracefully replaces an older local process while preserving its configuration, chats, and paired clients. Equal or newer running releases are reused. CLI, dashboard, and embedded desktop startup share the existing process lock and shutdown path.

Version discovery uses the existing authenticated gateway protocol, including a bounded retry for an older protocol version. Normal client protocol checks remain strict. The CLI waits for replacement to finish before reconnecting.

The public repository now contains the Rust framework, gateway, and CLI; native apps are maintained separately. The README uses the transparent Möbius mark.

Protocol 85, configuration 26, checkpoint payload 17, Bot SQLite schema 3, and session SQLite schema 10 are unchanged. Packages retain LICENSE, NOTICE, and pinned cloudflared.
