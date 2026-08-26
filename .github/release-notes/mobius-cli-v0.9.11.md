# möbius CLI 0.9.11

- Uses provider-default authentication whenever setup receives an explicit API key.
- Bundles möbius 0.9.11 and möbius Gateway 0.9.16.
- Remains on protocol 47; configuration 20 and chat specification 9 are unchanged.

Upgrade both installed commands together:

```sh
mobius-gateway exit
cargo install --force --locked mobius-cli --version 0.9.11
mobius-gateway serve --background
```
