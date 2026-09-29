# möbius CLI 0.15.53

Updates the terminal client and bundled gateway for protocol 87, catalog revisions and session updates, same-Bot chat messaging, and native Git line totals. Terminal interaction and saved account formats are unchanged.

Pins mobius 0.15.50 and mobius-gateway 0.15.54. Upgrade the gateway and all clients together: protocol versions other than 87 are rejected. Gateway configuration 26, chat specification 15, checkpoint payload 17, and SQLite schema 10 are unchanged. Stop the newer local gateway before a coordinated rollback to an older gateway and matching clients. Packages retain LICENSE and NOTICE.
