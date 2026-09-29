# möbius Gateway 0.15.54

Open chats belonging to the same Bot can discover and message each other. Peer messages steer an active turn or start a turn in an idle chat. The recipient checks its current Bot before accepting delivery, and senders receive agent rejections, including a full message queue. Hidden chats are excluded from discovery.

Protocol 86 now carries catalog revisions and ordered `sessions_changed` updates, representing unchanged chats by ID. Clients can supply known revisions or skip sections when authenticating, and unchanged Bot broadcasts are suppressed. `select_session` selects a chat for file, history, and workspace requests without replay or live events.

`get_git_diff_totals` returns Git's own added and removed line counts independently of the display diff limit, including untracked files for unstaged changes. Bot catalog reads reuse parsed state until database writes invalidate it, including writes from another connection.

Pins mobius 0.15.50. The protocol version stays 86, but clients built for the previous catalog behaviour do not understand these updates: upgrade the gateway and all clients together. Gateway configuration 26, chat specification 15, checkpoint payload 17, SQLite schema 10, Bot state 6, and Bot storage schema 3 are unchanged; no data reset is required. Packages retain LICENSE and NOTICE.
