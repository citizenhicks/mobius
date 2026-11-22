# mobius-gateway 0.17.15

New chats can route to an existing Bot or workspace through a configured decision model. Auto routing keeps the selected defaults if the decision times out, fails, or falls below the minimum confidence. Set the provider’s decision model IDs and configure Auto routing in Agent defaults.

Requires clients using protocol **105**. Upgrade remote clients and the gateway together.

## Upgrade from 0.17.14

Stop the old gateway and back up its state before starting 0.17.15. In `gateway.toml` Bot defaults and in each stored Möbius Bot composition in the `bots.sqlite3` catalog, add the missing integer `middleware.settings.clock.interval_minutes = 30`. Preserve the enabled middleware list and any explicit interval. Existing Bots can keep Clock off. Back up the checkpoint database with the same stopped state. The public release does not include a migration program. Remote clients and gateways must both use protocol 105.
