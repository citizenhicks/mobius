# mobius-desktop 0.17.15

Composer and transcript controls animate between glyphs. Chat recaps read bounded history on the existing authenticated gateway connection. New chats can use Auto routing, and question cards deliver answers and skips back to the agent.

macOS build **63** bundles gateway **0.17.15**. Remote connections require protocol **105**.

## Upgrade from 0.17.14

Stop the old gateway and back up its state before starting 0.17.15. In `gateway.toml` Bot defaults and in each stored Möbius Bot composition in the `bots.sqlite3` catalog, add the missing integer `middleware.settings.clock.interval_minutes = 30`. Preserve the enabled middleware list and any explicit interval. Existing Bots can keep Clock off. Back up the checkpoint database with the same stopped state. The public release does not include a migration program. Remote clients and gateways must both use protocol 105.
