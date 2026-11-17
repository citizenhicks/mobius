# mobius-gateway 0.17.11

- Broadcast effective operator policy, context accounting and typed events to thin clients, removing duplicated client checks and unnecessary requests.
- Apply sandboxed multi-file patches with stale-file checks, bounded preparation and accurate partial-commit reports.
- Release uniquely owned initial catalogs after sending them rather than retaining their full contents per connection.

**Coordinated upgrade required:** protocol **104** is incompatible with protocol 103 clients. Bot SQLite changes **6 → 7** and checkpoint SQLite **14 → 15**; config **30**, Bot state **10** and raw checkpoints **21** stay unchanged. Stop all writers, retain verified backups of both databases and perform a qualified offline conversion before starting this release. Historical error and token/context journal fields must be converted as well as the Bot table. Startup performs no migration; never delete unreadable state to force an upgrade.
