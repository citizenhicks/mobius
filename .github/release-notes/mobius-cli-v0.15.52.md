# möbius CLI 0.15.52

Updates the terminal client and bundled gateway for protocol 86 and encrypted WebSocket transport. WebSocket admission supports a separate bearer credential, validates the trusted gateway-service endpoint, and keeps that credential out of gateway protocol messages and agent subprocess environments.

Pins mobius 0.15.49 and mobius-gateway 0.15.53. Update the gateway and all clients together: protocol 85 clients and gateways are incompatible with this release. Existing WebSocket accounts must pair again using a new encryption-identity pairing code. Gateway configuration 26, chat specification 15, checkpoint payload 17, SQLite schema 10, and saved account formats are unchanged from the preceding releases.

Starting the newer local gateway replaces an older running version. Stop it before rolling back to an older gateway and matching clients. Preserve gateway data and `auth.channel-key`. Packages retain LICENSE and NOTICE.
