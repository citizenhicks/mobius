Core now exposes operational policy through typed, owner-local TOML defaults: model and OAuth transport timeouts, provider metadata/endpoints and pricing, token estimation, compaction and subagent ceilings, sandbox shell/environment/procfs settings, and tool-output/background-command budgets. API keys and static/session authorization headers borrow existing data; owned response and credential records move into their consumers.

Linux isolation keeps private user and PID namespaces in both explicit `ProcfsMode::Private` and `ProcfsMode::Empty` modes. Private procfs mount failures return an actionable error instead of weakening isolation. Configured tool-output limits now apply to errors as well as successful results.

Requires Rust 1.99.0. Shared typed configuration, native home discovery, ASCII identifiers, owner-only permission values and intentional mutex-poison recovery remove duplicated implementations. Gateway protocol 90 is unchanged.
