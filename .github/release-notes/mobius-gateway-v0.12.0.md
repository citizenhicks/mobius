## Highlights

- Introduces protocol 67 scoped contribution reads and actions, preserving extension references in global snapshots.
- Moves Scratchpad management grammar into its owning middleware.
- Shares session rename/pin persistence for active and stopped sessions, execution statistics, and provider endpoint selection.
- Serves refreshed workspace file catalogs to local and remote frontends, including bounded non-Git workspaces.
- Uses möbius 0.12.0, including GPT-6 Astra support.

## Upgrade

- Update the CLI and Apple app together with the gateway; this release requires protocol 67.
- Durable checkpoint and session data formats remain unchanged; action-list widgets are transient presentation records.
