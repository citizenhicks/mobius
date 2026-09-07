Moves Bot profiles and routine history from `bots.json` into transactional, owner-only `bots.sqlite3` storage. Gateway configuration advances from version 23 to 24, and Bot middleware gains explicit `routine_creation` permission. Existing configuration and Bot state require offline conversion before startup; there is no automatic migration. Back up state and preserve Bot IDs, routines, and run history during conversion.

Improves authorization, routine persistence, cancellation, and active-work reporting. Provider credential refresh now skips stopped resident sessions without preventing live sessions from receiving updates.

Chat metadata 15, checkpoint format 13, and wire protocol 71 are unchanged. Update the gateway before matching clients. Binary packages retain cloudflared, LICENSE, and NOTICE.
