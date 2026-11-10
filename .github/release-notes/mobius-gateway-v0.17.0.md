# mobius-gateway 0.17.0

Requires protocol **100** clients, framework **0.17.0**, and gateway configuration **30**. Upgrade the CLI and native apps together with the gateway. Protocol-99 clients cannot connect. Cloud and native operators must coordinate this cutover; do not erase or recreate gateway state to resolve a mismatch.

## Offline upgrade from 0.16.50

Stop every gateway writer and retain verified copies of the state directory and SQLite databases. Convert and validate a disposable copy first.

- Preserve the complete `[usage]` record from `gateway.toml` as owner-only `usage.json` beside it, encoded as JSON with the same `days` map and token totals. Only then remove the `[usage]` table and its nested tables from the TOML. Do not replace an existing nonempty usage file or discard recorded totals.
- Remove `oauth_callback_timeout_ms` and `oauth_callback_request_timeout_ms` from the model transport settings. Set the top-level configuration version to `30`.
- Convert custom `<state dir>/models/*.toml` catalogs to shared catalog-level reasoning presets with model references. Preserve model IDs, settings, selections and provider endpoints. Invalid old overrides otherwise fall back to bundled catalogs with a warning.
- Run the released `check-config` against the converted state before starting. Preserve pairing identity, credentials, Bots, routines, memory pages, checkpoints and attachments. Checkpoint SQLite schema remains **14**, Bot SQLite schema remains **6**; neither database is reset or migrated at startup.

Usage recording is stored independently of gateway configuration. Optional Bot collector metadata tables are still initialized when opening schema-six databases. New middleware settings are no longer silently added to stored Bot records. Protocol 100 consolidates credential, Git-diff and hook-trust requests, sends preview headers once, and carries streamed text in transcript operations.
