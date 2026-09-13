Project Bot semantics once at the gateway boundary, keep ready/catalog snapshots coherent, cache routine schedule work, and avoid holding global locks across client I/O. Routine restart, overlap, and daylight-saving behavior remain gateway-owned.

Wire protocol 81 requires clients to upgrade together. Before upgrading, convert checkpoint payload 16 to 17, rename persisted session-context `bot_id` fields to `owner_id`, and advance checkpoint SQLite schema 9 to 10 while the gateway is stopped.
