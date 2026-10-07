This release reduces repeated conversation serialization, checkpoint writes and unnecessary copies across the framework, gateway and CLI. It also simplifies compaction and strengthens state-publication and file-boundary handling.

**Existing state requires an explicit offline upgrade before starting the new gateway.** Checkpoint JSON advances to 19 and checkpoint SQLite to 13; gateway config advances to 27 and Bot state to 8. There is no automatic migration or legacy fallback. Stop all writers, retain verified backups and pilot a disposable copy using the [public upgrade instructions](https://github.com/citizenhicks/mobius/blob/mobius-v0.16.27/scripts/README-portable-upgrade.md). Removed encrypted native-compaction summaries cannot be recovered by the conversion; ordinary reasoning and transcript history remain preserved.

### CPU and memory

- Built-in OpenAI HTTP/WebSocket, Anthropic and Kimi paths retain the serialized request bytes checked against the request limit and send those same bytes. Replay changes may require re-encoding; authorization refresh reuses the admitted body.
- Provider serialization borrows history and schemas. Streamed tool results move through completion, unchanged calls reuse validated output, and queue handling avoids redundant snapshots while preserving rollback behavior.
- Model identity is checked once per token estimate, and tool visibility borrows its existing set until mutation is required.
- The ownership audit removed 280 method `.clone()` expressions relative to 0.16.26 using the same scanner. This is a source count, not a count of deep copies or a universal performance claim. Repetitive justification comments were removed while behavior and safety explanations remain.
- Release builds use measured thin LTO and one codegen unit.

### Disk and checkpoints

- Active context is stored in append-only indexed rows. Ordinary saves write the new suffix and checkpoint header; context rewrites atomically replace the active rows.
- Large matching text tool events reference their transcript output instead of storing another full copy. Public event replay reconstructs the complete output, preserving client behavior and manual forks.
- Per-session Weak identity hints avoid rereading warm histories on alternating saves. Cache misses use one ordered query, and prepared SQLite statements are reused.
- New databases support incremental page reclamation after compaction and session deletion. A 64 MiB journal-size setting limits retained WAL allocation after reset; it does not cap active writes. SQLite FULL durability remains enabled.

### Compaction and reliability

- A provider-neutral summary handoff replaces the native-compaction/offloading split. Context model identity distinguishes model changes from reasoning-effort changes, with provider-owned reasoning cleanup.
- Compaction preparation honors configured retry limits, bounded backoff and server `Retry-After`, with at most one transport fallback. It can fall back to the selected model when the previous route is unavailable. Oversized input and failed explicit requests no longer leave an endlessly retried handoff marker.
- Message staging, streamed-tool completion and checkpoint updates retain their failure and cancellation boundaries with fewer intermediate copies.
- Credential and configuration publication distinguish failure before replacement from an applied write whose durability could not be confirmed. File reads validate bounded regular files and sandbox access remains fail-closed.
- Library diagnostics use structured tracing; applications own filtering and subscribers.

### Measured results

On macOS arm64, 11 alternating pairs of a growing 24-tool loop versus installed 0.16.26 measured 81.7% fewer gateway disk-write bytes, 61.9% fewer CPU instructions and 21.2% lower peak RSS for the earlier serialization/ownership candidate. A subsequent 11-pair two-session comparison against that candidate measured a further 31.5% smaller checkpoint database, 11.3% fewer disk-write bytes and 19.0% fewer instructions, with overall latency effectively unchanged. The two percentages come from different workloads and must not be compounded.

These are isolated deterministic-provider measurements with complete request/history validation, not Linux/Cloud or universal speedup claims. Cached reads do not establish a physical-read improvement. Local framework tests, Clippy, documentation, migration checks and macOS app compilation passed before release review; platform validation remains a publication gate.

Protocol version remains 91. Rust 1.99.0 or newer is required.
