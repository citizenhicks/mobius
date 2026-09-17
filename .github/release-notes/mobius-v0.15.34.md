Keeps macOS attachment workspaces stable across reboots by identifying the APFS volume by its filesystem UUID instead of the mount device number. Workspace replacement checks remain inode-bound and fail closed.

Checkpoint format 17 and protocol records are unchanged from 0.15.32. Existing macOS `.attachment-workspace.json` markers must be rewritten with `volume_uuid` before upgrading; Linux markers and gateways are unchanged.
