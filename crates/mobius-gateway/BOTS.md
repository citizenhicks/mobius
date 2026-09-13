# Bots and context

The gateway owns Bot identity and execution. A Bot is a durable profile: its name,
handle, description, model, capabilities, permissions, and extensions apply to its
independent conversations. The gateway creates `@mobius` after provider setup.

## Context boundaries

| Surface | Durable context | Workspace | Visibility |
| --- | --- | --- | --- |
| Chat | One Bot's transcript and Agent checkpoint | User-selected | Chats catalog |
| Routine run | Fresh Agent transcript per invocation | Routine's pinned workspace | Routine history |
| Subagent | Child Agent transcript rooted in its parent | Parent workspace | Parent chat's task tree |

Every chat belongs to exactly one Bot. A Bot can search and read its own durable
conversation history through the sessions middleware, but retrieved text remains
evidence rather than new instructions. Hidden reasoning and private prompts are not
exposed.

## Routines

Routines are human-facing gateway resources, not Agent middleware or model tools.
They belong to one Bot and support one-time, interval, and cron schedules. Every
invocation uses a fresh hidden conversation, the Bot's current profile, and the
routine's pinned workspace. Results, failures, and transcripts remain in routine
history rather than the chat catalog.

## Subagents

Subagents are temporary specialists within an Agent's task tree. They use the parent
workspace and start without parent turns unless the caller explicitly forks history.
The parent can inspect a subagent while its own turn is running. `send_message`
delivers to running children or resumes completed and interrupted children with their
existing checkpoints.

Messages between a parent and subagent are typed message events. They retain their
author and source session, appear in the owning chat transcript, and remain peer
advice: they cannot grant permission or expand the receiving Agent's authority.

## Working state and shared knowledge

- **Compaction handoff** keeps the active task's constraints, progress, next steps,
  and history references. `write_handoff` and `new_context` preserve the running
  turn while replacing its active model window. Durable history remains readable.
- **Tasks** keeps the optional task list for that Agent session.
- **Scratchpad** stores concise reusable knowledge shared across gateway
  conversations. Agent writes require approval; humans manage notes in the UI.

Transient progress, private reasoning, raw outputs, and secrets do not belong in
shared notes.

## Ownership in code

- `src/middleware/sessions.rs`: Bot-scoped history search and reading tools.
- `src/middleware/scratchpad.rs`: global shared knowledge.
- `src/middleware/compaction.rs`, `tasks.rs`, and `subagents/`: Agent working state.
- `crates/mobius-gateway/src/bots/`: Bot profiles, routines, and routine history.
- `crates/mobius-gateway/src/host/`: chat and routine execution.
