Resolve user-installed skill directory symlinks to canonical resource roots, including shared skills linked between `.codex/skills` and `.agents/skills`.

Broken links and non-directory entries are ignored; explicit skills retain precedence. Workspace discovery and reads within skill directories remain confined, with regression tests covering aliases and escape protection.
