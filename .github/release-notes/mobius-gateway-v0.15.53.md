# möbius Gateway 0.15.53

WebSocket gateway connections now use a pinned Noise channel (`mobius-noise-v1`) inside TLS. Relays carry bounded binary ciphertext records; gateway authentication and commands are encrypted end to end. WebSocket admission bearer credentials remain separate from device pairing and are excluded from agent subprocess environments.

Protocol 86 requires updated clients; protocol 85 clients cannot connect. Existing WebSocket accounts must pair again using a new code containing the gateway's encryption identity. Existing authentication records remain intact, and opaque device tokens continue to authenticate updated TCP/TLS clients. The first launch adds an owner-only `auth.channel-key` beside `auth.json`; preserve that file with gateway state backups, since replacing it changes the identity pinned by paired clients.

The local desktop app can lend a browser page scoped to a chat. Browser requests have bounded queues and cancellation cleanup. Large completed protocol frames release their excess read-buffer allocation, and account debug output no longer exposes credentials.

Pins mobius 0.15.49. Gateway configuration 26, chat specification 15, checkpoint payload 17, and SQLite schema 10 are unchanged from 0.15.52; no data reset is required for that upgrade. Upgrade the gateway and clients together, and stop the newer gateway before any coordinated rollback. Packages retain LICENSE and NOTICE.
