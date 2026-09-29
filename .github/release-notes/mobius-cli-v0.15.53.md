# möbius CLI 0.15.53

Updates the terminal client and bundled gateway for catalog revisions and session updates, same-Bot chat messaging, and native Git line totals. Terminal interaction and saved account formats are unchanged.

Pins mobius 0.15.50 and mobius-gateway 0.15.54. The protocol version stays 86, but clients built for the previous catalog behaviour do not understand these updates: upgrade the gateway and all clients together. Gateway configuration 26, chat specification 15, checkpoint payload 17, and SQLite schema 10 are unchanged. Stop the newer local gateway before a coordinated rollback to an older gateway and matching clients. Packages retain LICENSE and NOTICE.
