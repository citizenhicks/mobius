# mobius 0.17.14

- Move the built-in OpenAI Live catalog to GPT Live 1.1. Calls wake on speech and return to standby after 15 seconds without speech on the same WebRTC connection; `idle_timeout_s` in the voice catalog accepts 5–60 seconds.
- Check still-image formats once, before an upload is accepted, using the same rules as decoding. Image MIME types are matched case-insensitively.
- Move HTTP clients to reqwest 0.13 with the ring TLS provider. Certificates are now verified against the operating system's trust store instead of a bundled root list, so Linux hosts need a CA certificate package such as `ca-certificates`.
- Upgrade bundled SQLite through rusqlite 0.43 and refresh compatible dependencies.

Upgrading from 0.17.12 or 0.17.11 preserves protocol **104** and the existing checkpoint format.
