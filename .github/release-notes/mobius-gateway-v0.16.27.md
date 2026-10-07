The gateway adopts the framework's CPU, memory and storage improvements, with fewer repeated history copies and checkpoint writes across concurrent sessions. See the [framework release notes](https://github.com/citizenhicks/mobius/releases/tag/mobius-v0.16.27) for the full compaction, ownership, reliability and benchmark details.

**Upgrade state before starting this gateway.** Config version is 27, Bot state is 8, checkpoint JSON is 19 and checkpoint SQLite is 13. The runtime does not convert old state. Stop writers, retain backups and test the [manual upgrade](https://github.com/citizenhicks/mobius/blob/mobius-gateway-v0.16.27/scripts/README-portable-upgrade.md) on a disposable copy first. Protocol version remains 91.

- Append-only context rows avoid serializing and rewriting the full active conversation at every save. Per-session hints and cached SQL reduce repeated parsing and statement preparation.
- Large matching tool events share transcript storage while history replay still returns complete outputs. Incremental vacuum reclaims deleted pages, and retained WAL allocation is limited to 64 MiB after reset. FULL durability is unchanged.
- Gateway session, catalog, provider and subagent paths borrow data or move it into its final owner. Publication and rollback handling preserve visible state when a later durability step fails.
- Structured tracing replaces direct library printing. The gateway entry point installs the subscriber; `RUST_LOG` controls filtering.
- The coordinated release profile uses thin LTO and one codegen unit for both binaries.

In the final two-session fixture, 11 alternating pairs versus the preceding schema-12 candidate measured 31.5% smaller databases, 11.3% fewer disk-write bytes, 19.0% fewer CPU instructions and 2.1% lower median peak RSS. Full tool outputs and history matched; task latency was essentially unchanged. These macOS fixture results do not predict savings for every existing database, and no live database was migrated for the benchmark.
