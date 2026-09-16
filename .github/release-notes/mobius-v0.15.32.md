Makes the active sandbox mode authoritative for file reads and writes. Full access now permits absolute host paths for `read_file`, `view_image`, `write_file`, `apply_patch`, artifacts, and computer observations, while workspace mode retains pinned-root and symlink confinement.

This release changes the public `SandboxBackend` file-operation signatures to receive `SandboxMode`. Checkpoint format 17 and protocol records are unchanged from 0.15.31.
