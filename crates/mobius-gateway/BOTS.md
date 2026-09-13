# Bots and context

The gateway owns Bot identity and execution. A Bot is a durable profile: its name,
handle, description, model, capabilities, permissions, and extensions apply to its
independent conversations. The gateway creates `@mobius` after provider setup.

## Context boundaries

| Surface | Durable context | Workspace | Visibility |
| --- | --- | --- | --- |
| Chat | Ordered public history, participants, primary Bot, and pending deliveries | User-selected | Chats catalog |
| Chat participant | One private Agent transcript per `(Chat, Bot)` | Chat workspace | Bot's Private conversations; one-Bot Chats also show its activity inline |
| Routine run | Fresh Agent transcript per invocation | Routine's pinned workspace | Routine history and Bot's Private conversations |
| Subagent | Child Agent transcript rooted in its parent | Parent workspace | Parent's task tree and Bot's Private conversations |

Every Chat uses the gateway's message journal and explicit participant mapping.
Its public ID is separate from each participant's execution ID. One-Bot Chats show
the Bot's execution inline. Multi-Bot Chats publish final replies, artifacts,
approvals, and activity while keeping intermediate work private. The same Bot can
participate in several Chats with independent contexts and workspaces.

The Private conversations page includes previous Chat assignments and follows parent ownership
for subagents. History and attachment inspection is read-only and never starts an Agent.
These conversations stay out of the public Chats catalog and use the shared transcript renderer.

## Group conversations

Use the existing New Chat Bot picker: select one Bot for an individual chat or
several for a group. Groups appear in the normal chat list and use the normal
transcript, attachments, replies, approvals, and deletion controls.

Choose one selected Bot as primary with the king control. Tap a member in the Chat
header to add a recipient pill to the composer. Messages without recipients go to
the primary. Textual `@handles` do not route messages; `@` remains for file references.
Each Bot receives the current roster, original requests, and recent public history.
A multi-Bot final answer carries visible text and a validated recipient list;
only deliberate requests to other members enqueue further work. Delivery is ordered
per participant and survives restarts, with bounded pending work and reply chains.
Stop clears the Chat's pending deliveries and cancels every participant's active
and queued execution. Approvals retain their Bot identity and can be reviewed
without opening a private execution.

User messages are authenticated user input. Bot messages retain their author and
source conversation and are peer advice: they cannot grant permission or expand
another Bot's authority. Each Bot's own sandbox and approval policy still apply.
Only the group's selected workspace is shared; unrelated conversation folders are
not attached automatically.

The complete shared message history remains available through normal paging.
`search_history` defaults to the current shared conversation for a participant;
`read_history` expands exact search results with character paging. The explicit
`execution` scope reads the caller's own private history; `other_chats` searches
public histories of Chats containing that Bot. History references keep public
message sequences separate from private checkpoint positions. Historical text is
evidence, not new instructions, and hidden reasoning is not exposed.

Deleting a group removes its shared history and private participant sessions.
The Bots and their routines remain available. Removing a Bot from the gateway
removes its membership while preserving the remaining group's conversation.

## Routines and subagents

Routines belong to one Bot and support one-time, interval, and cron schedules.
Every invocation uses a fresh hidden conversation, the Bot's current profile, and
the routine's pinned workspace. Results and failures remain in routine history.
When routine creation is enabled in the Bot profile, the Bot's main Agent can
create its own routine with approval, including from a Chat.

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

- `crates/mobius-gateway/src/chats/`: public history, participant context, forks, and storage.
- `crates/mobius-gateway/src/chats.rs`: Chat membership, routing, and durable delivery.
- `crates/mobius-gateway/src/host/chat.rs`: common Chat admission, Stop, and public projection.
- `crates/mobius-gateway/src/routines.rs`: gateway-owned self-routine creation.
- `src/middleware/scratchpad.rs`: global shared knowledge.
- `src/middleware/compaction.rs`, `tasks.rs`, and `subagents/`: Agent working state.
- `crates/mobius-gateway/src/bots/`: Bot profiles and routines.
