Public chats now use one durable chat journal while each Bot keeps an independent private checkpoint. Existing single-Bot and group conversations can be converted without changing their public chat IDs, titles, pinning, or message history.

Chat participants, pending delivery, routing, and attachment projection are owned by the chat store. Bot configuration separates chat settings from private session settings and moves routine creation policy to the Bot configuration root.

Checkpoint payload 15, chat SQLite 2, Bot SQLite 2/state 5, gateway config 25, and wire protocol 79 are intentionally incompatible with 0.15.21. Back up and migrate gateway state before upgrading.
