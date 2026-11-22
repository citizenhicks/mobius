# mobius-cli 0.17.15

The terminal client supports Auto routing for new chats, decision model IDs in provider settings, and asynchronous question cards. It bundles gateway **0.17.15** and uses protocol **105**. Update a remote gateway before connecting with this CLI.

## Upgrade from 0.17.14

Stop the old gateway and back up its state before starting 0.17.15. In `gateway.toml` Bot defaults and in each stored Möbius Bot composition in the `bots.sqlite3` catalog, add the missing integer `middleware.settings.clock.interval_minutes = 30`. Preserve the enabled middleware list and any explicit interval. Existing Bots can keep Clock off. Back up the checkpoint database with the same stopped state. The public release does not include a migration program. Remote clients and gateways must both use protocol 105.
