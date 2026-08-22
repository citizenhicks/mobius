# möbius 0.9.8

- Keeps portable Agent Plugin v1 and legacy Codex packages focused on skills and hooks.
- Rejects MCP and app contributions instead of silently installing a partially supported plugin.
- Preserves host Git configuration, credential helpers, SSH identities, and SSH agents inside the command sandbox while suppressing repository-local Git execution redirects.

Upgrade: removes the experimental `AppsAuthorization`/`apps_authorization`, remote-MCP extension metadata and `Extensions::with_tools`, and unused `ToolContext::call_id` APIs introduced in 0.9.7.

Gateway protocol 45, configuration 20, and chat specification 9 are supported by the companion Gateway release.
