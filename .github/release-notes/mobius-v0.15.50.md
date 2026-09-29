# möbius 0.15.50

The sessions middleware can expose `list_chats` and `message_chat` to main agents through a host-provided `LiveChats` interface. Agents can discover their owner's other open chats and address peer messages by full session ID or short `#id`. Subagents retain the existing history tools without these live-chat tools.

Checkpoint payload 17 and SQLite schema 10 are unchanged. Packages retain LICENSE and NOTICE.
