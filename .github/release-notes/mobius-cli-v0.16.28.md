CLI and bundled gateway advance together to 0.16.28 with protocol 92 and per-model provider catalogs.

**Existing 0.16.27 gateway config needs an offline 27 → 28 conversion before launch.** Bot and checkpoint formats are unchanged from 0.16.27. Follow the [manual upgrade instructions](https://github.com/citizenhicks/mobius/blob/mobius-cli-v0.16.28/scripts/README-portable-upgrade.md); installation and startup do not migrate state.

- Setup accepts editable providers that also advertise a seed catalog, including Anthropic, Kimi and DeepSeek. It selects the entered model and uses that model's configured reasoning choices and default.
- New catalog-backed setups start from their advertised model list. Existing setup edits preserve model metadata and omitted reasoning settings in the gateway.
- Untouched tint and image IDs are omitted from registration, avoiding stale client copies and unnecessary image-list allocations.
- The bundled gateway exposes `--models-json`, Native/Rebuild `--tool-discovery`, `--preserve-selection` and `--if-configured` for repeatable automation. Per-model reasoning replaces the retired shared `--reasoning-efforts` flag.

See the [gateway notes](https://github.com/citizenhicks/mobius/releases/tag/mobius-gateway-v0.16.28) for configuration and registration semantics. Both binary archives retain manual pages, licenses and the pinned cloudflared bundle.
