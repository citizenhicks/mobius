Exposes Automatic/Handoff compaction and optional Bot collaboration through the existing capability catalog. Handoff excludes context offloading while Tasks remain independently optional. Search/read history is scoped to the current chat or the owning Bot and includes original tool arguments and results.

Gateway protocol 70 adds policy-exclusion and Bot collaboration-eligibility metadata. Upgrade clients and gateways together. Existing configurations need explicit compaction.mode and bots.collaboration settings; existing Swarm catalogs need the removed routine-projection bookkeeping field removed. Operational migrations must preserve chats, credentials, shared notes, and existing Swarm membership. No automatic runtime migration is included.

Binary packages include pinned cloudflared, LICENSE, and NOTICE.
