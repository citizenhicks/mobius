# möbius for macOS 0.10.27

Bundles gateway 0.16.46, including idle compaction for large chats and fixes for activity, notifications, hooks and history when a message arrives during compaction.

Background compaction no longer triggers a normal chat-completion reaction. Protocol 97 and stored-state formats remain unchanged. Gateway data already converted for 0.16.44 needs no further conversion.
