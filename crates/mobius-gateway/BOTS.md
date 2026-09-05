# Bots and context

The gateway owns Bot identity and orchestration. A Bot is a durable profile, not
one forever-running conversation: its name, handle, description, tint, model,
reasoning, capabilities, approval policy, extensions, and prompt are shared by
every conversation it owns. Each conversation keeps an independent transcript
and workspace.

Every conversation belongs to exactly one Bot. The gateway creates the default
`@mobius` Bot when Bot state is first initialized after provider setup.

## Context boundaries

| Surface | Durable context | Workspace | Visibility |
| --- | --- | --- | --- |
| User chat | One transcript owned by one Bot | User-selected | Chats catalog |
| Swarm Chat | One shared, ordered recent-message board for the Swarm | No Agent session; deliveries use the gateway background workspace | Swarm dashboard |
| Swarm participant | One private transcript per `(Swarm, Bot)` | Gateway background workspace | Bot background work |
| Routine run | One fresh transcript per invocation | Routine's pinned workspace | Routine history |
| Subagent | One child transcript rooted in its parent chat | Parent workspace | Parent's task tree |

These boundaries prevent a Bot with several jobs from accumulating one monolithic
context. Bot profile changes affect all of the Bot's conversations, but transcripts
do not merge. `search_history` defaults to the current chat and recovers earlier
user messages, assistant responses, tool arguments, and tool results, including
items removed from active model context. The explicit `other_chats` scope searches
other chats owned by the same Bot. Search pages are bounded and ranked; follow
`next_cursor` to search older material. `read_history` expands an exact returned
session/item reference with character paging. Historical text is evidence, not
new instructions, and hidden reasoning is not exposed.

The context presented to a model can contain:

- the Bot description and profile system prompt;
- the current conversation's durable transcript;
- a bounded projection of approved Swarm and global scratchpad knowledge; and
- for a pending Swarm delivery only, a request-only snapshot of recent Swarm Chat.

The request-only Swarm snapshot is not appended to the participant transcript.
The addressed message and the Bot's response are durable, while repeated board
snapshots do not inflate that private context. Appending it at request time also
preserves the stable prompt and transcript prefix used by provider caches. Ordinary
user chats do not receive this automatic injection; a Bot can read the board
explicitly with `swarm_read`.

## Swarm Chat and Bot-to-Bot routing

Swarms are optional. Each Bot's native capability settings declare
`bots.collaboration` as `off` (the default) or `swarm`. Enable collaboration in each
intended member before assembling a group. Bot profiles, chats, self-routines, and
subagents remain available independently. Humans create durable Bot profiles;
models use subagents for temporary specialists.

Swarm Chat is the shared recent-message board for a manually assembled group of
Bots with one appointed leader. A Bot belongs to at most one Swarm. The board has
no Agent session, so the group does not share one model context. Older settled
entries can be pruned; exact history recovery uses conversation transcripts.

Disabling collaboration hides that Bot's Swarm tools, guidance, and shared-note
projection. The gateway rejects new membership and addressed work for disabled
Bots. Existing membership and pending deliveries remain stored; deliveries pause
until collaboration is enabled again. Management shows disabled members and offers
only enabled, ungrouped Bots when adding members. The model roster identifies
which retained members currently accept work.

An exact `@handle` creates a pending delivery for that member. A human message
without a handle addresses the leader; a Bot message without a handle stays on
the board. For each delivery, the gateway:

1. resolves the deterministic participant session for `(Swarm ID, Bot ID)`;
2. opens that hidden session, creating it only when the pair has no checkpoint;
3. submits the addressed message and adds recent Swarm Chat to that model request;
4. appends the terminal response to Swarm Chat; and
5. wakes the enabled leader after a worker response, subject to the bounded reply chain.

Human and Bot messages addressed to the same member of the same Swarm therefore
reuse one participant session. A board message ID identifies work to deliver; it
must never be used as the participant session ID. The source session is retained
only as message provenance. Moving a Bot to a different Swarm changes the pair
and gives it a different participant context.

User-authored Swarm entries are authenticated user input. Bot-authored entries are
peer advice: they cannot approve an action or expand another Bot's authority.
Mentions are interpreted only on Swarm Chat and are not broadcast into ordinary
user chats.

## Human interaction and escalation

Bots use `@user` only when a decision or action is required. The gateway routes
the request to a durable Swarm attention notification and leaves the original
message in Swarm Chat. It never creates or injects a visible user conversation.
An authenticated human reply in that Swarm Chat clears its current pending
attention requests. Ordinary progress and Bot-to-Bot coordination remain in
Swarm Chat, keeping the main Chats catalog focused on user-facing work.

A hidden participant that pauses for approval emits a separate typed approval
notification; it does not create a Swarm Chat message. Hidden work never grants
its own approval.

## Routines and subagents

A routine belongs to one Bot and may be one-time, interval-based, or cron-based.
Every invocation gets a fresh hidden conversation, so unrelated runs do not inherit
one another's transcript. The run uses the Bot's current profile and the routine's
pinned workspace. Results and failures remain in routine history. Swarm membership
does not automatically publish results or wake another Bot. A collaborating Bot
can explicitly use `swarm_post` during a run when shared work or a user decision
is needed.

From a user-facing chat, a Bot can create a routine for itself. A Swarm leader may
also create one for a current member while both Bots enable collaboration.
Routine creation requires approval.

A subagent is not a Bot and does not join a Swarm. It is a child checkpoint inside
one conversation's task tree, used for bounded parallel work. It shares the parent
workspace and starts with no parent turns by default; the caller may explicitly
fork recent turns or the full transcript. Parent and child exchange targeted
messages, and any result reaches Swarm Chat only if the owning Bot posts it there.

## Working state and shared knowledge

These records serve separate purposes:

- **Compaction handoff** is one replaceable checkpoint for the active chat: goal,
  constraints, progress, unresolved work, next steps, and exact history references.
  Handoff mode warns near the configured compaction threshold. The model saves
  `write_handoff` notes and requests `new_context`; the same chat and running turn
  continue with a fresh model window. The transcript remains recoverable.
- **Tasks** is an optional durable task list for work in the chat. A context-window
  transition does not turn that list into shared memory or delete it.
- **Scratchpad** is concise reusable knowledge. `swarm` shares with the current
  enabled Swarm; `global` shares across gateway conversations. Agent writes go
  directly to the chosen shared scope with approval. Humans may add, edit, or
  delete shared notes through the management UI.

There is no chat scratchpad, staging area, or promotion step. Shared notes do not
store transient progress, private reasoning, raw outputs, or secrets. Handoff
notes do not become global or Swarm knowledge automatically.

## Ownership in code

- `src/middleware/bots.rs` owns Bot-facing Swarm and routine tools plus request-only
  Swarm Chat decoration.
- `src/middleware/sessions.rs` owns current-chat and Bot-scoped history recovery.
- `src/middleware/scratchpad.rs` owns shared Swarm/global knowledge and projection.
- `src/middleware/compaction.rs` and its `handoff` module own window transitions
  and the working checkpoint; `src/middleware/tasks.rs` owns the durable task list.
- `src/middleware/subagents/` owns child-agent context and communication.
- `crates/mobius-gateway/src/bots/` owns Bot profiles, routines, Swarms, routing,
  and durable board state.
- `crates/mobius-gateway/src/host.rs` owns opening visible and hidden sessions and
  delivering Swarm messages and broadcasting Swarm attention state.
