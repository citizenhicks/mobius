# mobius-cli 0.17.11

- Render sandboxed multi-file patch diffs and committed file links, including accurate partial-failure results.
- Consume gateway-owned policy and typed events, avoiding redundant validation and requests.
- Preserve multi-file diff boundaries when output is truncated or hunk text resembles a file header.

Bundles gateway **0.17.11** and requires protocol **104**. Existing Bot/checkpoint databases need the coordinated offline conversion described in the [gateway upgrade notes](https://github.com/citizenhicks/mobius/releases/tag/mobius-gateway-v0.17.11) before use. Protocol-103 gateways are incompatible.
