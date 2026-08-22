# möbius Gateway 0.9.12

- Removes the experimental remote MCP and extension-auth runtime, configuration, and wire protocol.
- Adds safe Git credential status and setup with username-only readback through the host credential helper.
- Adds safe SSH identity inventory and create-new Ed25519 setup without exposing private keys through the frontend.
- Keeps portable extension installation, updates, removal, and hook trust, with unsupported plugin contributions rejected explicitly.

Gateway protocol 45, configuration 20, and chat specification 9 are supported. Configuration 19 is intentionally incompatible; back up and recreate or manually transform Gateway state before upgrading.
