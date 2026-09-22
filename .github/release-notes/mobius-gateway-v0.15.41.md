Fix gateway startup and chat creation failing with `a path led outside of the filesystem` when a user-installed skill directory is a symlink.

Includes mobius 0.15.40. Workspace/plugin confinement is unchanged; this patch does not change the protocol, checkpoint payload, or database schema.
