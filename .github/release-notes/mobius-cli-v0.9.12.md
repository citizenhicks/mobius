# möbius CLI 0.9.12

- Removes the retired conversational cron setup path.
- Applies scratchpad commands during active turns when the shared store is available.
- Bundles möbius 0.9.12 and möbius Gateway 0.9.17 using protocol 48.
- Requires Rust 1.98 or newer when installed from source.

Upgrade both installed commands together:

```sh
mobius-gateway exit
cargo install --force --locked mobius-cli --version 0.9.12
mobius-gateway serve --background
```
