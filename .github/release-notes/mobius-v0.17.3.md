# mobius 0.17.3

Approval decisions are written to the event journal in the same transaction as the
approval checkpoint. Question widgets carry the prompt and outcome separately,
including multiline prompts.

Rust integrations that construct `FrontendActionListItem` with a struct literal
must set its new `outcome` field. Use `None` for rows without an outcome.
