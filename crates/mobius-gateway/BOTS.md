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
| Swarm Chat | One shared, ordered board for the Swarm | No Agent session; deliveries use the gateway background workspace | Swarm dashboard |
| Swarm participant | One private transcript per `(Swarm, Bot)` | Gateway background workspace | Bot background work |
| Routine run | One fresh transcript per invocation | Routine's pinned workspace | Routine history |
| Subagent | One child transcript rooted in its parent chat | Parent workspace | Parent's task tree |

These boundaries prevent a Bot with several jobs from accumulating one monolithic
context. Bot profile changes affect all of the Bot's conversations, but transcripts
do not merge. A Bot may explicitly search its other threads with `search_threads`;
the search is Bot-scoped and BM25-ranked, and its results are retrieved rather than
silently inserted into the current context.

The context presented to a model can contain:

- the Bot description and profile system prompt;
- the current conversation's durable transcript;
- a bounded projection of session, Swarm, and global scratchpad notes; and
- for a pending Swarm delivery only, a request-only snapshot of recent Swarm Chat.

The request-only Swarm snapshot is not appended to the participant transcript.
The addressed message and the Bot's response are durable, while repeated board
snapshots do not inflate that private context. Appending it at request time also
preserves the stable prompt and transcript prefix used by provider caches. Ordinary
user chats do not receive this automatic injection; a Bot can read the board
explicitly with `swarm_read`.

## Swarm Chat and Bot-to-Bot routing

Swarm Chat is the shared durable transcript for a manually assembled group of
Bots with one appointed leader. A Bot belongs to at most one Swarm. It is a board,
not an Agent session, so messages can be read by the whole Swarm without making
every Bot share one model context.

An exact `@handle` creates a pending delivery for that member. A human message
without a handle addresses the leader; a Bot message without a handle stays on
the board. For each delivery, the gateway:

1. resolves the deterministic participant session for `(Swarm ID, Bot ID)`;
2. opens that hidden session, creating it only when the pair has no checkpoint;
3. submits the addressed message and adds recent Swarm Chat to that model request;
4. appends the terminal response to Swarm Chat; and
5. wakes the leader after a worker response, subject to the bounded reply chain.

Human and Bot messages addressed to the same member of the same Swarm therefore
reuse one participant session. A board message ID identifies work to deliver; it
must never be used as the participant session ID. The source session is retained
only for causal routing and user escalation. Moving a Bot to a different Swarm
changes the pair and gives it a different participant context.

User-authored Swarm entries are authenticated user input. Bot-authored entries are
peer advice: they cannot approve an action or expand another Bot's authority.
Mentions are interpreted only on Swarm Chat and are not broadcast into ordinary
user chats.

## Human interaction and escalation

Bots use `@user` only when a decision or action is required. The gateway routes
the escalation back to the existing visible source conversation when that causal
chat is suitable. If the source was hidden or came directly from Swarm Chat, the
gateway creates a visible leader-owned conversation instead. Ordinary progress
and Bot-to-Bot coordination remain in Swarm Chat, keeping the main Chats catalog
focused on user-facing work.

A hidden participant that pauses for approval also projects that need to Swarm
Chat and into the same escalation path. Hidden work never grants its own approval.

## Routines and subagents

A routine belongs to one Bot and may be one-time, interval-based, or cron-based.
Every invocation gets a fresh hidden conversation, so unrelated runs do not inherit
one another's transcript. The run uses the Bot's current profile and the routine's
pinned workspace. When its Bot is in a Swarm, the terminal result is projected to
Swarm Chat; a worker result wakes the leader, while work needing a user decision
uses the escalation path.

From a user-facing chat, a Bot can create a routine for itself. A Swarm leader may
also create one for a current member. Routine creation requires approval.

A subagent is not a Bot and does not join a Swarm. It is a child checkpoint inside
one conversation's task tree, used for bounded parallel work. It shares the parent
workspace and starts with no parent turns by default; the caller may explicitly
fork recent turns or the full transcript. Parent and child exchange targeted
messages, and any result reaches Swarm Chat only if the owning Bot posts it there.

## Scratchpads

Scratchpad notes are concise durable conclusions, not model reasoning or raw
output. Their scopes are:

- **session**: available only to the exact conversation;
- **Swarm**: shared by the current Swarm's members; and
- **global**: available to every gateway conversation.

An agent writes to its session scratchpad and may copy an exact note to the Swarm
or global scope with approval. A human may add, edit, or delete shared notes from
the management UI. Promotion copies a note; it does not merge transcripts or move
the original note.

## Ownership in code

- `src/middleware/bots.rs` owns Bot-facing Swarm and routine tools plus request-only
  Swarm Chat decoration.
- `src/middleware/sessions.rs` owns Bot-scoped thread search.
- `src/middleware/scratchpad.rs` owns all three scratchpad scopes and projection.
- `src/middleware/subagents/` owns child-agent context and communication.
- `crates/mobius-gateway/src/bots/` owns Bot profiles, routines, Swarms, routing,
  and durable board state.
- `crates/mobius-gateway/src/host.rs` owns opening visible and hidden sessions and
  delivering Swarm messages and user escalations.
