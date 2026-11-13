# mobius 0.17.8

Coordinated release with the gateway, CLI, and macOS app.

Expose committed execution statistics through `Agent::subscribe_run_stats` and
`CommittedRunStats`. `RunStats` and `RunSummary` move into the framework and become
non-exhaustive; downstream struct literals and exhaustive matches must be updated.
The gateway and clients use protocol **102** for live statistics.

`ToolContext::report_usage` now takes the paid operation's model route before its
usage, keeping the tool's foreground model identity unchanged.

Builds pin Rust 1.100.0 and opt into its experimental eight-worker compiler frontend.
The minimum supported Rust version is 1.100.0.
