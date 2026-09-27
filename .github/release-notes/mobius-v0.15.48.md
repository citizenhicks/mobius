# möbius 0.15.48

Computer control announces browser-viewing availability through a capability-owned frontend widget and removes stale browser links after worker failure, timeout, or cancellation.

Frontend blocks now carry capability-owned resource links, including file links for coding tools. This adds `FrontendLink` and the `FrontendBlock.links` field to the public framework API. Checkpoint payload 17 and SQLite schema 10 are unchanged.
