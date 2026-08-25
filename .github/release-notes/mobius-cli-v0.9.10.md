# möbius CLI 0.9.10

- Bundles möbius 0.9.10 and möbius Gateway 0.9.15 with the corrected scheduled-task execution flow.
- Uses protocol 47; configuration 20 and chat specification 9 are unchanged.

Upgrade both installed commands together:

```sh
mobius-gateway exit
cargo install --force --locked mobius-cli --version 0.9.10
mobius-gateway serve --background
```
