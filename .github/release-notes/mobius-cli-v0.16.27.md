The CLI and bundled gateway are updated together to 0.16.27, including the framework's lower-copy model requests, append-only checkpoints, tool-output storage deduplication and simplified compaction.

**Existing gateway state needs an offline upgrade before launch.** Starting the new gateway does not migrate old configuration or history. Follow the [manual upgrade instructions](https://github.com/citizenhicks/mobius/blob/mobius-cli-v0.16.27/scripts/README-portable-upgrade.md), retain backups and pilot a disposable copy. Protocol version remains 91.

- Terminal catalog, message rendering, account selection and command paths avoid unnecessary owned copies.
- Provider setup preserves the original Bot/template revision across registration, so concurrent edits are rejected instead of silently overwritten.
- The executable entry point is reduced to argument parsing and dispatch; application behavior stays in the CLI library and frontend modules.
- The bundled gateway uses structured, filterable diagnostics and retains complete public history events despite deduplicated disk storage.
- Both binaries use the measured thin-LTO release profile. Earlier profile comparisons reduced CLI/gateway binary size by about 26%/29% versus the same cleanup built with its former profile; build-size results are separate from runtime memory measurements.

See the [framework notes](https://github.com/citizenhicks/mobius/releases/tag/mobius-v0.16.27) and [gateway notes](https://github.com/citizenhicks/mobius/releases/tag/mobius-gateway-v0.16.27) for CPU, memory, disk, compaction, reliability and migration details. The final two-session workload preserved complete outputs while reducing checkpoint size by 31.5% and disk writes by 11.3% versus the preceding candidate; elapsed task time was essentially unchanged. Measurements are workload-specific and were run on macOS.
