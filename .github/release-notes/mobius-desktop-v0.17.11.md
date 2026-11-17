# mobius-desktop 0.17.11

- Display sandboxed multi-file patch diffs and written-file links, including partial-commit reports.
- Consume gateway policy and typed events instead of repeating checks and requests in the client.
- Correct truncated multi-file diff parsing and provider usage refresh races.

Desktop build **60** bundles gateway **0.17.11** and requires protocol **104**. Remote protocol-103 gateways cannot connect. Existing local Bot/checkpoint databases require the qualified offline conversion in the [gateway upgrade notes](https://github.com/citizenhicks/mobius/releases/tag/mobius-gateway-v0.17.11); startup does not migrate state. Coordinate gateway and client upgrades.
