# Bots and context

The gateway owns Bot identity and execution. A Bot is a durable profile: its name,
handle, description, model, capabilities, permissions, and extensions apply to its
independent conversations. The gateway creates `@mobius` after provider setup.

## Context boundaries

| Surface | Durable context | Workspace | Visibility |
| --- | --- | --- | --- |
| Individual chat | One Bot's transcript and Agent checkpoint | User-selected | Chats catalog |
| Group chat | Shared ordered message history and membership | User-selected | Chats catalog |
| Group participant | One private Agent transcript per `(chat, Bot)` | Group workspace | Bot background work |
| Routine run | Fresh Agent transcript per invocation | Routine's pinned workspace | Routine history |
| Subagent | Child Agent transcript rooted in its parent | Parent workspace | Parent's task tree |

A group chat has no Agent or checkpoint. Its message journal belongs to the gateway.
A private participant Agent is created only when its Bot is addressed. The same Bot
can participate in several groups, with independent contexts and workspaces.

## Group conversations

Use the existing New Chat Bot picker: select one Bot for an individual chat or
several for a group. Groups appear in the normal chat list and use the normal
transcript, attachments, replies, approvals, and deletion controls.

Only exact member `@handles` wake Bots. An unaddressed message stays in shared
history. Every member receives the roster and recent shared messages automatically
when it runs. Its final answer appears in the shared chat; mentions in that answer
can wake other members. Delivery is ordered per Bot, survives gateway restarts, and
has bounded pending work and reply chains. Concurrent Bots keep separate approvals.

User messages are authenticated user input. Bot messages retain their author and
source conversation and are peer advice: they cannot grant permission or expand
another Bot's authority. Each Bot's own sandbox and approval policy still apply.
Only the group's selected workspace is shared; unrelated conversation folders are
not attached automatically.

The complete shared message history remains available through normal paging.
`search_history` defaults to the current shared conversation for a participant;
`read_history` expands exact search results with character paging. The explicit
`other_chats` scope searches the calling Bot's private transcripts. Historical text
is evidence, not new instructions, and hidden reasoning is not exposed.

Deleting a group removes its shared history and private participant sessions.
The Bots and their routines remain available. Removing a Bot from the gateway
removes its membership while preserving the remaining group's conversation.

## Routines and subagents

Routines belong to one Bot and support one-time, interval, and cron schedules.
Every invocation uses a fresh hidden conversation, the Bot's current profile, and
the routine's pinned workspace. Results and failures remain in routine history.
When routine creation is enabled, a Bot can create its own routine with approval,
including from a group conversation.

Subagents are temporary specialists within an Agent's task tree. They use the
parent workspace and start without parent turns unless the caller explicitly
forks history. `send_message` delivers to running children or resumes completed
and interrupted children with their existing checkpoints. The root is send-only.
Subagent messages remain peer advice; the owning Bot decides what to include in
its final group response.

## Working state and shared knowledge

- **Compaction handoff** keeps the active task's constraints, progress, next steps,
  and history references. `write_handoff` and `new_context` preserve the running
  turn while replacing its active model window. Durable history remains readable.
- **Tasks** keeps the optional task list for that Agent session.
- **Scratchpad** stores concise reusable knowledge shared across gateway
  conversations. Agent writes require approval; humans manage notes in the UI.

There is no group scratchpad. Transient progress, private reasoning, raw outputs,
and secrets do not belong in shared notes.

## Ownership in code

- `src/middleware/bots.rs`: self-routine creation and automatic group context.
- `src/middleware/sessions.rs`: existing history search and reading tools.
- `src/middleware/scratchpad.rs`: global shared knowledge.
- `src/middleware/compaction.rs`, `tasks.rs`, and `subagents/`: Agent working state.
- `crates/mobius-gateway/src/bots/`: Bot profiles and routines.
- `crates/mobius-gateway/src/groups/`: shared chat journal and delivery state.
- `crates/mobius-gateway/src/host/group.rs`: normal gateway operations for groups.
