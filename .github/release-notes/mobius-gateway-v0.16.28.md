Provider setups now share one configuration and registration path, with reasoning choices and defaults stored per model.

**Upgrade config before starting this gateway.** Gateway config advances from 27 to 28 and the wire protocol from 91 to 92. Bot state, checkpoint JSON and checkpoint SQLite remain at 8, 19 and 13. Existing 0.16.27 installations need only the offline config conversion. Stop all writers, retain verified backups and follow the [manual upgrade instructions](https://github.com/citizenhicks/mobius/blob/mobius-gateway-v0.16.28/scripts/README-portable-upgrade.md). Runtime startup does not migrate old state. Use protocol-92 clients.

- Only Codex and OpenAI Socket lock model catalogs. New editable catalog-backed setups seed their advertised models and reasoning; saved setups thereafter own their lists.
- Full model metadata is editable in configuration. Paired clients can edit IDs and reasoning while preserving operator-owned labels, descriptions and context windows; the local operator can change those fields through registration.
- Provider-level Native/Rebuild discovery overrides are validated and applied by the transport. Endpoint, authentication and web-search routing uses live setup settings.
- `register-provider --models-json` sets per-model reasoning/defaults and optional metadata; repeatable `--model-id` and `--image-model-id` remain supported. The shared `--reasoning-efforts` flag is removed.
- `--preserve-selection` refreshes endpoints/credentials without overwriting live catalogs or selections. `--if-configured` skips absent optional setups under the gateway mutation lock.
- Omitted labels, tint and image IDs preserve live values instead of resending disk snapshots. Omitted/empty model lists preserve existing catalogs; an explicit empty image list clears images.
- Config conversion seeds existing editable provider catalogs from matching owner manifests, preserves metadata and overrides, and rejects malformed inputs before writing.
