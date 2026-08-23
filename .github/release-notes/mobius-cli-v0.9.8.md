# möbius CLI 0.9.8

- Adds native clipboard file and image attachments to the terminal composer with bounded, correlated uploads.
- Bundles möbius 0.9.8 and möbius Gateway 0.9.13 using protocol 46.

Upgrade both installed commands together:

```sh
mobius-gateway exit
cargo install --force --locked mobius-cli --version 0.9.8
mobius-gateway serve --background
```
