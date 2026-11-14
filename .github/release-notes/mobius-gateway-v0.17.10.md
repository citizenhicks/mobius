# mobius-gateway 0.17.10

- Release a chat no client holds from memory after `connections.idle_session_minutes` (default 30, 1–1440) with no events or admitted messages; it reloads from its checkpoint on the next message, so resident memory follows the chats in use rather than the session limit. [312f40d5](https://github.com/citizenhicks/mobius/commit/312f40d5)
- List each session file with `modified_at_ms`, when its stored copy was last written. [af4f3f90](https://github.com/citizenhicks/mobius/commit/af4f3f90)
- Include the framework's cached memory pages and shared memory read lock. [188001cf](https://github.com/citizenhicks/mobius/commit/188001cf), [67a2414c](https://github.com/citizenhicks/mobius/commit/67a2414c)

Protocol is now **103**: protocol 102 clients, including the 0.17.8 macOS app, the 0.17.9 CLI and the released iOS app, cannot connect. Update clients together with the gateway. Configuration **30** and checkpoint **21** are unchanged, so 0.17.8 and 0.17.9 gateway state upgrades in place without conversion; `idle_session_minutes` is optional.
