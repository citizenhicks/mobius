# mobius 0.17.10

- Keep each memory scope's parsed pages between model calls and re-read a scope only when a page file is added, removed, resized or touched; at most 64 scopes stay cached. Two full scopes (400 pages of 16 KB) went from 24.6 ms to 3.7 ms per call in a release build. [188001cf](https://github.com/citizenhicks/mobius/commit/188001cf)
- Let memory reads share the store lock: model calls, searches and dream reads run together, and only patches and edits are exclusive. [67a2414c](https://github.com/citizenhicks/mobius/commit/67a2414c)
- Record when each stored session file was last written. [af4f3f90](https://github.com/citizenhicks/mobius/commit/af4f3f90)

Coordinated with gateway, CLI and macOS app **0.17.10** on protocol **103**. For library users, `SessionFileRecord` gains the required `modified_at_ms` field (Unix milliseconds).
