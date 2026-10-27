# mobius-gateway 0.16.45

Includes framework 0.16.45 with idle compaction for large chats. Gateways finish active turns and scheduled idle compaction before shutting down. Maintenance preserves the chat preview and does not trigger turn-completion hooks. Protocol 97, gateway configuration 29, Bot catalog 10, checkpoint 21 and SQLite schema 14 are unchanged from 0.16.44; this patch needs no further record conversion.
