# möbius CLI 0.9.9

- Uses gateway-advertised web-search choices and session-file limits while retaining client safety ceilings.
- Renders cron output through the generic capability presentation path.
- Keeps setup, dashboard, terminal sanitization, and endpoint ownership in focused frontend modules.
- Bundles möbius 0.9.9 and möbius Gateway 0.9.14 using protocol 47.

Upgrade both installed commands together:

```sh
mobius-gateway exit
cargo install --force --locked mobius-cli --version 0.9.9
mobius-gateway serve --background
```
