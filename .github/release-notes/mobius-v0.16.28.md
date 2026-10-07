Provider model configuration now uses one full model record for built-in and configured catalogs, including independent reasoning choices, an explicit default, context window and tool discovery.

- Codex and OpenAI Socket retain locked model catalogs. Anthropic, Kimi, DeepSeek, OpenRouter and Responses accept operator-configured models and reasoning efforts.
- Provider constructors apply validated Native/Rebuild discovery overrides consistently. Kimi advertises Rebuild only and rejects unsupported Native mode before a request.
- Editable providers no longer inject catalog reasoning defaults over a configured model's explicit no-reasoning choice.
- Catalog routing borrows model records instead of building intermediate copies. Registration moves existing model records and reasoning metadata into their replacement configuration.

This patch introduces public Rust model/build configuration fields; update struct literals when upgrading. The coordinated gateway uses protocol 92 and config version 28. Rust 1.99.0 or newer is required. The history, storage and compaction improvements from 0.16.27 remain unchanged; no new performance percentage is claimed for this patch.
