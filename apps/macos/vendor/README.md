# Vendored UI dependencies

GPUI Kit's `gpui-base`, `gpui-component` and `gpui-kit-assets` 0.9.1 are the
published Apache-2.0 crates. Only Cargo.toml changes: remove the unused gpui-pre
and gpui-pre-platform dependencies and route inspector/test-support to Fast.
Their Rust code and assets are unchanged. The application selects `gpui-fast`.
Keep licenses and notices when updating these crates.
