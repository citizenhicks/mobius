# mobius 0.17.15

## Agent tools and middleware

- Tools can return a deferred response. The agent can continue working and receive the outcome for that invocation later. Answering or skipping an `ask_user` question now delivers an explicit result.
- The optional Clock middleware adds a time note when due and keeps it in the conversation for prompt caching. Existing Bots can enable it in their capabilities.
- Authors of custom tools should return `ToolResponse::Completed` or `ToolResponse::Deferred` from the tool interface.

## Upgrade from 0.17.14

Stop the old gateway and back up its state before starting 0.17.15. In `gateway.toml` Bot defaults and in each stored Möbius Bot composition in the `bots.sqlite3` catalog, add the missing integer `middleware.settings.clock.interval_minutes = 30`. Preserve the enabled middleware list and any explicit interval. Existing Bots can keep Clock off. Back up the checkpoint database with the same stopped state. The public release does not include a migration program. Remote clients and gateways must both use protocol 105.
