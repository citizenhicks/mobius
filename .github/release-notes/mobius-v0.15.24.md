Restore the direct one-Bot-per-chat session model and remove group-chat routing and Bot collaboration middleware. Routines remain gateway-owned resources, while subagent messages keep their typed peer identity and transcript presentation.

Immediate first-turn messages no longer publish a transient queued widget. Checkpoint payload 16 is intentionally incompatible with 0.15.23; migrate gateway state before upgrading.
