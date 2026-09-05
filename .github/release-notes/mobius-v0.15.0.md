Adds model-directed handoff compaction with durable working checkpoints and fresh context windows in the same chat. Original messages and tool results remain recoverable through current-chat and own-Bot history search/read tools. Context transitions retain active user corrections, completed tool batches, loaded tools, and capability-owned Tasks state.

Scratchpad now contains shared Swarm/global notes with approved direct writes. Swarm collaboration is opt-in per Bot; permanent Bot creation and automatic routine-result dispatch were removed. Declarative policy exclusions prevent Handoff from running with context offloading.

This minor release changes public middleware, checkpoint query, and configuration APIs. Gateway protocol is 70. Runtime compatibility shims and automatic state migrations are not included.
