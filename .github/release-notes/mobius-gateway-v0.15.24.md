Restore one Bot and one Agent checkpoint per chat. Group chats and their shared delivery system are removed; routines remain gateway-owned and their transcripts stay in routine history. Capacity admission retains the completed-command retry fix.

This release requires an offline state conversion from 0.15.23. It upgrades gateway config 25 to 26, Bot SQLite 2/state 5 to SQLite 3/state 6, checkpoint payload 15 to 16, removes the chat database, and moves public chats back to the checkpoint catalog. Wire protocol 80 requires clients to upgrade together.
