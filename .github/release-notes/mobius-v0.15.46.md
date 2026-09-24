# möbius 0.15.46

- Move provider model catalogs, pricing, middleware prompts, defaults, and image wire mappings into typed owner-local TOML.
- Use GPT Image 2.5 Sunburst for image generation.
- Simplify shell tools to `bash` and `manage_command`: quick commands complete inline; longer commands return an ID for polling or stopping.
- Preserve sandbox approvals, session ownership, bounded output, and cancellation during command startup.
- Let tools own their prompts, model exposure, and transcript presentation through the existing Tool trait.
- Keep attachment repair in its middleware and fork cleanup in protocol replay.

Framework API changes include owned model presets, default-value accessors, and the pre-commit compaction preparation hook. Tool callers should use `manage_command` with `action: "poll"` or `"stop"` in place of the removed command tools.
